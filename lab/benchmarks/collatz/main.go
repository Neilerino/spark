package main

import "fmt"

func collatzLen(n int64) int64 {
	steps := int64(0)
	for n != 1 {
		if n%2 == 0 {
			n = n / 2
		} else {
			n = 3*n + 1
		}
		steps++
	}
	return steps
}

func main() {
	total := int64(0)
	for n := int64(1); n <= 5_000_000; n++ {
		total += collatzLen(n)
	}
	fmt.Println(total)
}
