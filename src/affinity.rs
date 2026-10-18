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
}
