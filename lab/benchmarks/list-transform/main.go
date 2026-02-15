package main

import "fmt"

func main() {
	const n = 1_000_000

	nums := make([]int64, n)
	for i := range nums {
		nums[i] = int64(i + 1)
	}

	squared := make([]int64, n)
	for i, v := range nums {
		squared[i] = v * v
	}

	modded := make([]int64, n)
	for i, v := range squared {
		modded[i] = v % 1000
	}

	total := int64(0)
	for _, v := range modded {
		total += v
	}

	fmt.Println(total)
}
