use std::thread;

fn main() {
    let n: i64 = 10_000_000;
    let num_workers = thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4) as i64;
    let chunk_size = n / num_workers;

    let mut handles = Vec::new();

    for w in 0..num_workers {
        let start = w * chunk_size;
        let end = if w == num_workers - 1 { n } else { start + chunk_size };

        handles.push(thread::spawn(move || {
            let mut sum = 0i64;
            for i in start..end {
                sum += (i * i) % 1000;
            }
            sum
        }));
    }

    let mut total = 0i64;
    for h in handles {
        total += h.join().unwrap();
    }

    println!("{}", total);
}
