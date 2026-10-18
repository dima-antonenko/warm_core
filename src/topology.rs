//! Топология CPU из sysfs: физические ядра, пары SMT и размеры кэшей.
//!
//! На i7-8750H L1/L2 общие только у пары hyperthread (`0,6`, `1,7`, …),
//! L3 — один на весь кристалл. Worker закрепляется за одним логическим CPU
//! физического ядра, чтобы второй SMT-поток не делил с ним L1 и L2.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Идентификатор физического ядра (`core_id` из sysfs), не логический CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CoreId(pub u32);

/// Один логический CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    pub logical_id: usize,
    pub core_id: CoreId,
    pub package_id: u32,
    /// Логические CPU, которые делят с этим ядром L1 и L2.
    pub siblings: Vec<usize>,
}

/// Один уровень кэша, как его описывает sysfs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheInfo {
    pub level: u32,
    pub kind: String,
    pub size_bytes: u64,
    pub line_size: usize,
    pub shared_cpus: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Topology {
    pub cpus: Vec<Cpu>,
    pub caches: Vec<CacheInfo>,
    /// Размер линии, к которой выравниваются индексы очереди. Обычно 64.
    pub cache_line: usize,
}

impl Topology {
    pub fn detect() -> io::Result<Self> {
        Self::from_sysfs("/sys/devices/system/cpu")
    }

    pub fn from_sysfs(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let mut cpus = Vec::new();

        let mut entries = fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());

        for entry in entries {
            let Some(logical_id) = logical_cpu_id(&entry.file_name().to_string_lossy()) else {
                continue;
            };
            let topo = entry.path().join("topology");
            if !topo.join("core_id").exists() {
                continue;
            }
            let core_id = read_u32(topo.join("core_id"))?;
            let package_id = read_u32(topo.join("physical_package_id")).unwrap_or(0);
            let siblings_raw = read_trimmed(topo.join("thread_siblings_list"))
                .or_else(|_| read_trimmed(topo.join("core_cpus_list")))
                .unwrap_or_else(|_| logical_id.to_string());
            let siblings = parse_cpu_list(&siblings_raw)?;
            cpus.push(Cpu {
                logical_id,
                core_id: CoreId(core_id),
                package_id,
                siblings,
            });
        }

        if cpus.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "в sysfs не найдены CPU",
            ));
        }
        cpus.sort_by_key(|cpu| cpu.logical_id);

        let caches = read_caches(&root.join("cpu0/cache")).unwrap_or_default();
        let cache_line = caches
            .iter()
            .map(|cache| cache.line_size)
            .find(|size| *size > 0)
            .unwrap_or(64);

        Ok(Self {
            cpus,
            caches,
            cache_line,
        })
    }

    /// Один логический CPU на физическое ядро — младший id из SMT-пары.
    ///
    /// Второй hyperthread делит L1 и L2. Свой worker на него не сажаем,
    /// чтобы чужой поток того же ядра не вымывал линии нашего worker.
    pub fn physical_workers(&self) -> Vec<Cpu> {
        let mut cpus = self.cpus.clone();
        cpus.sort_by_key(|cpu| (cpu.core_id.0, cpu.package_id, cpu.logical_id));
        let mut chosen = Vec::new();
        for cpu in cpus {
            if chosen
                .iter()
                .any(|kept: &Cpu| kept.core_id == cpu.core_id && kept.package_id == cpu.package_id)
            {
                continue;
            }
            chosen.push(cpu);
        }
        chosen.sort_by_key(|cpu| cpu.logical_id);
        chosen
    }

    pub fn logical_workers(&self) -> Vec<Cpu> {
        let mut cpus = self.cpus.clone();
        cpus.sort_by_key(|cpu| cpu.logical_id);
        cpus
    }

    pub fn l2_size_bytes(&self) -> Option<u64> {
        self.caches
            .iter()
            .find(|cache| cache.level == 2)
            .map(|cache| cache.size_bytes)
    }
}

fn logical_cpu_id(name: &str) -> Option<usize> {
    let id = name.strip_prefix("cpu")?;
    if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    id.parse().ok()
}

pub(crate) fn parse_cpu_list(text: &str) -> io::Result<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_cpu_index(start)?;
            let end = parse_cpu_index(end)?;
            if end < start {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("перевёрнутый диапазон CPU: {part}"),
                ));
            }
            cpus.extend(start..=end);
        } else {
            cpus.push(parse_cpu_index(part)?);
        }
    }
    Ok(cpus)
}

fn parse_cpu_index(text: &str) -> io::Result<usize> {
    text.trim().parse().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("некорректный номер CPU «{text}»: {err}"),
        )
    })
}

fn read_caches(dir: &Path) -> io::Result<Vec<CacheInfo>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut indexes = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(index) = name.strip_prefix("index") else {
            continue;
        };
        let Ok(index) = index.parse::<u32>() else {
            continue;
        };
        indexes.push((index, entry.path()));
    }
    indexes.sort_by_key(|(index, _)| *index);

    let mut caches = Vec::with_capacity(indexes.len());
    for (_, path) in indexes {
        caches.push(read_cache(&path)?);
    }
    Ok(caches)
}

fn read_cache(path: &Path) -> io::Result<CacheInfo> {
    let level = read_u32(path.join("level"))?;
    let kind = read_trimmed(path.join("type")).unwrap_or_else(|_| "Unknown".to_string());
    let size_raw = read_trimmed(path.join("size")).unwrap_or_else(|_| "0".to_string());
    let line_raw =
        read_trimmed(path.join("coherency_line_size")).unwrap_or_else(|_| "64".to_string());
    let shared_raw = read_trimmed(path.join("shared_cpu_list")).unwrap_or_default();
    Ok(CacheInfo {
        level,
        kind,
        size_bytes: parse_size(&size_raw)?,
        line_size: parse_cpu_index(&line_raw)?,
        shared_cpus: parse_cpu_list(&shared_raw)?,
    })
}

pub(crate) fn parse_size(text: &str) -> io::Result<u64> {
    let text = text.trim();
    let (number, multiplier) = if let Some(number) = text.strip_suffix(['K', 'k']) {
        (number, 1024)
    } else if let Some(number) = text.strip_suffix(['M', 'm']) {
        (number, 1024 * 1024)
    } else if let Some(number) = text.strip_suffix(['G', 'g']) {
        (number, 1024 * 1024 * 1024)
    } else {
        (text, 1)
    };
    let number = number.trim();
    let value: u64 = number.parse().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("некорректный размер кэша «{text}»: {err}"),
        )
    })?;
    Ok(value.saturating_mul(multiplier))
}

fn read_trimmed(path: PathBuf) -> io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn read_u32(path: PathBuf) -> io::Result<u32> {
    let text = read_trimmed(path)?;
    text.parse().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ожидалось число, получено «{text}»: {err}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cpu_lists() {
        assert_eq!(parse_cpu_list("0,6").unwrap(), vec![0, 6]);
        assert_eq!(parse_cpu_list("0-3,8").unwrap(), vec![0, 1, 2, 3, 8]);
        assert_eq!(parse_cpu_list(" 1-2, 4 ").unwrap(), vec![1, 2, 4]);
        assert!(parse_cpu_list("5-1").is_err());
    }

    #[test]
    fn parses_sysfs_cache_sizes() {
        assert_eq!(parse_size("32K").unwrap(), 32 * 1024);
        assert_eq!(parse_size("256K").unwrap(), 256 * 1024);
        assert_eq!(parse_size("9216K").unwrap(), 9216 * 1024);
        assert_eq!(parse_size("9M").unwrap(), 9 * 1024 * 1024);
    }

    #[test]
    fn detects_host_topology() {
        let topology = Topology::detect().unwrap();
        assert!(!topology.cpus.is_empty());
        assert!(topology.cache_line.is_power_of_two());

        let physical = topology.physical_workers();
        assert!(!physical.is_empty());
        assert!(physical.len() <= topology.cpus.len());

        let mut cores = physical.iter().map(|cpu| cpu.core_id).collect::<Vec<_>>();
        let before = cores.len();
        cores.sort();
        cores.dedup();
        assert_eq!(
            cores.len(),
            before,
            "на одно физическое ядро приходится один worker"
        );
    }
}
