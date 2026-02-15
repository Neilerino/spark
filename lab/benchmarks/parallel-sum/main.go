package main

import (
	"fmt"
	"runtime"
	"sync"
)

func main() {
	const n = 10_000_000
	numWorkers := runtime.NumCPU()
	chunkSize := n / numWorkers

	results := make([]int64, numWorkers)
	var wg sync.WaitGroup

	for w := 0; w < numWorkers; w++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			start := id * chunkSize
			end := start + chunkSize
			if id == numWorkers-1 {
				end = n
			}
			sum := int64(0)
			for i := start; i < end; i++ {
				v := int64(i)
				sum += (v * v) % 1000
			}
			results[id] = sum
		}(w)
	}

	wg.Wait()

	total := int64(0)
	for _, v := range results {
		total += v
	}
	fmt.Println(total)
}
