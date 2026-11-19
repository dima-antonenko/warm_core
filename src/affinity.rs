//! Привязка потока к одному логическому CPU.
//!
//! Маска из одного CPU не даёт ядру перенести поток. L1 и L2 этого ядра
//! остаются заняты его working set, пока поток сам с них не уйдёт.

use std::io;
use std::mem;

pub fn pin_to(cpu: usize) -> io::Result<()> {
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        let rc = libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set);
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Предпочитать узел NUMA этого CPU для новых анонимных страниц.
///
/// `pin_to` фиксирует, где исполняется поток, но не переносит уже выделенную
/// память. Политика стоит после привязки: следующие fault'ы оседают на узле CPU.
/// Ошибку вызывающий может игнорировать: песочница иногда запрещает syscall,
/// а привязка к CPU при этом всё равно работает.
pub(crate) fn prefer_node(node: u32) -> io::Result<()> {
    let node = node as usize;
    let word = node / usize::BITS as usize;
    let mut mask = vec![0usize; word + 1];
    mask[word] |= 1usize << (node % usize::BITS as usize);
    let maxnode = mask.len() * usize::BITS as usize;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_set_mempolicy,
            libc::c_int::from(1), // MPOL_PREFERRED
            mask.as_ptr(),
            maxnode as libc::c_ulong,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Узел NUMA, на котором сейчас исполняется поток. `None`, если syscall недоступен.
pub(crate) fn current_numa_node() -> Option<u32> {
    let mut cpu = 0u32;
    let mut node = 0u32;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_getcpu,
            &mut cpu,
            &mut node,
            std::ptr::null::<libc::c_void>(),
        )
    };
    if rc == 0 { Some(node) } else { None }
}

/// CPU, на которых потоку разрешено исполняться.
#[cfg(test)]
pub(crate) fn allowed_cpus() -> io::Result<Vec<usize>> {
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_ZERO(&mut set);
        let rc = libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &mut set);
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut cpus = Vec::new();
        for cpu in 0..libc::CPU_SETSIZE as usize {
            if libc::CPU_ISSET(cpu, &set) {
                cpus.push(cpu);
            }
        }
        Ok(cpus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn pin_restricts_the_affinity_mask_to_one_cpu() {
        let cpu = allowed_cpus().unwrap()[0];
        let mask = thread::spawn(move || {
            pin_to(cpu).unwrap();
            allowed_cpus().unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(mask, vec![cpu]);
    }

    #[test]
    fn prefer_node_accepts_the_node_the_thread_is_on() {
        let node = current_numa_node().expect("getcpu");
        thread::spawn(move || prefer_node(node).unwrap())
            .join()
            .unwrap();
    }
}
