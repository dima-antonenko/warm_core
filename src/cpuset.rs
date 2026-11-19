//! CPU и узлы памяти, которые cgroup разрешает этому процессу.
//!
//! sysfs показывает все CPU машины. В контейнере процесс может исполняться
//! только на подмножестве: это `cpuset.cpus.effective` и `cpuset.mems.effective`.
//! Файл часто лежит не в листовой группе, а у предка, поэтому поиск идёт вверх
//! до точки монтирования.

use crate::topology::parse_cpu_list;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Limits {
    /// `None` — ограничение cpuset не нашлось, годятся все CPU из sysfs.
    pub cpus: Option<Vec<usize>>,
    /// `None` — ограничение по узлам памяти не нашлось.
    pub mems: Option<Vec<u32>>,
}

pub(crate) fn current() -> Limits {
    let cgroup = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    from_mounts(&cgroup, &mounts, Path::new("/"))
}

pub(crate) fn from_mounts(cgroup: &str, mountinfo: &str, root: &Path) -> Limits {
    let mounts = parse_mountinfo(mountinfo);
    let mut cpu_sets = Vec::new();
    let mut mem_sets = Vec::new();

    for (controllers, path) in cgroup_lines(cgroup) {
        if controllers.is_empty() {
            let Some(mount) = best_mount(&mounts, "cgroup2", None, path) else {
                continue;
            };
            push_list(
                &mut cpu_sets,
                read_controller(root, mount, path, &["cpuset.cpus.effective"]),
            );
            push_list(
                &mut mem_sets,
                read_controller(root, mount, path, &["cpuset.mems.effective"]),
            );
        } else if controllers.contains(&"cpuset") {
            let Some(mount) = best_mount(&mounts, "cgroup", Some("cpuset"), path) else {
                continue;
            };
            push_list(
                &mut cpu_sets,
                read_controller(root, mount, path, &["cpuset.effective_cpus", "cpuset.cpus"]),
            );
            push_list(
                &mut mem_sets,
                read_controller(root, mount, path, &["cpuset.effective_mems", "cpuset.mems"]),
            );
        }
    }

    Limits {
        cpus: intersect_all(cpu_sets),
        mems: intersect_all(mem_sets).map(|ids| {
            ids.into_iter()
                .filter_map(|id| u32::try_from(id).ok())
                .collect()
        }),
    }
}

fn push_list(sets: &mut Vec<Vec<usize>>, text: Option<String>) {
    let Some(text) = text else {
        return;
    };
    if let Ok(list) = parse_cpu_list(&text) {
        sets.push(list);
    }
}

fn intersect_all(sets: Vec<Vec<usize>>) -> Option<Vec<usize>> {
    let mut sets = sets.into_iter();
    let mut acc = sets.next()?;
    for next in sets {
        acc.retain(|id| next.contains(id));
    }
    acc.sort_unstable();
    acc.dedup();
    Some(acc)
}

struct Mount {
    root: String,
    mount_point: String,
    fstype: String,
    options: String,
}

fn parse_mountinfo(text: &str) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for line in text.lines() {
        let Some((front, back)) = line.split_once(" - ") else {
            continue;
        };
        let front: Vec<&str> = front.split_whitespace().collect();
        let back: Vec<&str> = back.split_whitespace().collect();
        if front.len() < 5 || back.is_empty() {
            continue;
        }
        mounts.push(Mount {
            root: unescape_mount(front[3]),
            mount_point: unescape_mount(front[4]),
            fstype: back[0].to_string(),
            options: back.get(2).copied().unwrap_or("").to_string(),
        });
    }
    mounts
}

fn cgroup_lines(text: &str) -> Vec<(Vec<&str>, &str)> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let Some((_, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((controllers, path)) = rest.split_once(':') else {
            continue;
        };
        let controllers = if controllers.is_empty() {
            Vec::new()
        } else {
            controllers.split(',').collect()
        };
        lines.push((controllers, path));
    }
    lines
}

fn best_mount<'a>(
    mounts: &'a [Mount],
    fstype: &str,
    option: Option<&str>,
    cgroup_path: &str,
) -> Option<&'a Mount> {
    mounts
        .iter()
        .filter(|mount| mount.fstype == fstype)
        .filter(|mount| {
            option
                .map(|needle| mount.options.split(',').any(|option| option == needle))
                .unwrap_or(true)
        })
        .filter(|mount| relative_to_mount(&mount.root, cgroup_path).is_some())
        .max_by_key(|mount| mount.root.len())
}

fn relative_to_mount(mount_root: &str, cgroup_path: &str) -> Option<String> {
    let mount_root = trim_slash(mount_root);
    let cgroup_path = trim_slash(cgroup_path);
    if mount_root.is_empty() {
        return Some(cgroup_path.to_string());
    }
    if cgroup_path == mount_root {
        return Some(String::new());
    }
    cgroup_path
        .strip_prefix(mount_root)?
        .strip_prefix('/')
        .map(str::to_string)
}

fn trim_slash(path: &str) -> &str {
    path.trim_matches('/')
}

fn read_controller(
    root: &Path,
    mount: &Mount,
    cgroup_path: &str,
    names: &[&str],
) -> Option<String> {
    let relative = relative_to_mount(&mount.root, cgroup_path)?;
    let mount_point = under_root(root, &mount.mount_point);
    let start = if relative.is_empty() {
        mount_point.clone()
    } else {
        mount_point.join(relative)
    };
    read_upwards(&start, &mount_point, names)
}

fn under_root(root: &Path, absolute: &str) -> PathBuf {
    let relative = absolute.trim_start_matches('/');
    if relative.is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    }
}

fn read_upwards(start: &Path, stop: &Path, names: &[&str]) -> Option<String> {
    let mut path = start.to_path_buf();
    loop {
        for name in names {
            let file = path.join(name);
            if file.is_file() {
                return fs::read_to_string(file)
                    .ok()
                    .map(|text| text.trim().to_string());
            }
        }
        if path == *stop || !path.pop() {
            break;
        }
    }
    None
}

fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 3 < bytes.len() {
            let octal = &text[index + 1..index + 4];
            if let Ok(byte) = u8::from_str_radix(octal, 8) {
                out.push(byte);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("warm-core-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, body: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn effective_cpus_come_from_an_ancestor() {
        let root = TempDir::new("cpuset-v2");
        write(
            &root
                .0
                .join("sys/fs/cgroup/user.slice/cpuset.cpus.effective"),
            "0-1,4\n",
        );
        write(
            &root
                .0
                .join("sys/fs/cgroup/user.slice/cpuset.mems.effective"),
            "0\n",
        );
        write(
            &root
                .0
                .join("sys/fs/cgroup/user.slice/app/leaf/cgroup.procs"),
            "",
        );
        let limits = from_mounts(
            "0::/user.slice/app/leaf\n",
            "20 1 0:20 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n",
            &root.0,
        );
        assert_eq!(limits.cpus, Some(vec![0, 1, 4]));
        assert_eq!(limits.mems, Some(vec![0]));
    }

    #[test]
    fn v1_and_v2_sets_are_intersected() {
        let root = TempDir::new("cpuset-hybrid");
        write(&root.0.join("sys/fs/cgroup/cpuset.cpus.effective"), "0-5\n");
        write(&root.0.join("sys/fs/cgroup/cpuset.mems.effective"), "0-1\n");
        write(
            &root.0.join("sys/fs/cgroup/cpuset/docker/cpuset.cpus"),
            "1-3\n",
        );
        write(
            &root.0.join("sys/fs/cgroup/cpuset/docker/cpuset.mems"),
            "1\n",
        );
        let limits = from_mounts(
            "0::/\n3:cpuset:/docker\n",
            "\
20 1 0:20 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw
21 1 0:21 / /sys/fs/cgroup/cpuset rw - cgroup cgroup rw,cpuset
",
            &root.0,
        );
        assert_eq!(limits.cpus, Some(vec![1, 2, 3]));
        assert_eq!(limits.mems, Some(vec![1]));
    }

    #[test]
    fn host_cgroup_v2_reports_a_cpu_list() {
        let cgroup = fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        if !cgroup.lines().any(|line| line.starts_with("0::")) {
            return;
        }
        let limits = current();
        let cpus = limits.cpus.expect("у cgroup v2 нет списка CPU");
        assert!(!cpus.is_empty());
    }
}
