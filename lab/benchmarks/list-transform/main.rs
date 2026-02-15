fn main() {
    let nums: Vec<i64> = (1..=1_000_000).collect();
    let squared: Vec<i64> = nums.iter().map(|x| x * x).collect();
    let modded: Vec<i64> = squared.iter().map(|x| x % 1000).collect();

    let mut total = 0i64;
    for v in &modded {
        total += v;
    }

    println!("{}", total);
}
