//! Два ядра, у каждого свой буфер.
//!
//! Буфер создаётся в `shard_on` на том worker, который будет им владеть.
//! Внешний поток читает его через `call`. Одно ядро просит другое дописать
//! значение через `defer` и получает длину встречным `defer`. `call` с
//! worker на worker здесь не используется: взаимное ожидание двух ядер
//! не разрывается, пока каждое спит внутри своего `call`.

use std::sync::mpsc;
use std::time::Duration;

use warm_core::Pool;

fn current_cpu() -> i32 {
    unsafe { libc::sched_getcpu() }
}

fn main() {
    let pool = Pool::new().expect("пул");
    let cores = pool.cores();
    if cores.len() < 2 {
        println!("нужны два ядра, сейчас {}", cores.len());
        return;
    }
    let left_core = cores[0];
    let right_core = cores[1];

    let left = pool
        .shard_on(left_core, || {
            println!("init левого шарда на CPU {}", current_cpu());
            Vec::<u64>::new()
        })
        .expect("левый шард");
    let right = pool
        .shard_on(right_core, || {
            println!("init правого шарда на CPU {}", current_cpu());
            Vec::<u64>::new()
        })
        .expect("правый шард");

    let (tx, rx) = mpsc::channel();
    let right_for_left = right.clone();
    let left_for_right = left.clone();
    left.call(move |mine| {
        mine.push(1);
        right_for_left
            .defer(move |theirs| {
                theirs.push(2);
                let len = theirs.len() as u64;
                left_for_right
                    .defer(move |mine| {
                        mine.push(len);
                        let _ = tx.send(mine.clone());
                    })
                    .expect("ответ");
            })
            .expect("запрос");
    })
    .expect("локальная запись");

    let mine = rx.recv_timeout(Duration::from_secs(2)).expect("ответ");
    println!("левый буфер после ответа: {mine:?}");
    assert_eq!(mine, vec![1, 1]);

    let seen = left.call(|mine| mine.clone()).expect("чтение снаружи");
    let remote = right.call(|theirs| theirs.clone()).expect("чтение соседа");
    println!("снаружи: левый {seen:?}, правый {remote:?}");
    assert_eq!(seen, vec![1, 1]);
    assert_eq!(remote, vec![2]);
}
