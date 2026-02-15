fn collatz_len(mut n: i64) -> i64 {
    let mut steps = 0i64;
    while n != 1 {
        if n % 2 == 0 {
            n /= 2;
        } else {
            n = 3 * n + 1;
        }
        steps += 1;
    }
    steps
}

fn main() {
    let mut total = 0i64;
    for n in 1..=5_000_000 {
        total += collatz_len(n);
    }
    println!("{}", total);
}
