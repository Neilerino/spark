import multiprocessing


def compute_chunk(args):
    start, end = args
    total = 0
    for i in range(start, end):
        total += (i * i) % 1000
    return total


def main():
    n = 10_000_000
    num_workers = multiprocessing.cpu_count()
    chunk_size = n // num_workers
    chunks = []
    for w in range(num_workers):
        start = w * chunk_size
        end = start + chunk_size if w < num_workers - 1 else n
        chunks.append((start, end))

    with multiprocessing.Pool(num_workers) as pool:
        results = pool.map(compute_chunk, chunks)

    print(sum(results))


if __name__ == "__main__":
    main()
