package main

func scaled(x uint32) uint32 { return x * 7 }

//go:noinline
func calculate(x uint32) uint32 {
	return scaled(x) + 11 // controlled edit: 11 -> 12
}

//go:wasmexport entry
func entry(x uint32) uint32 { return calculate(x) }

func main() {}
