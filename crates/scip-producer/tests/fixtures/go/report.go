package main

import "example.com/fixture/geometry"

func report(p geometry.Point) float64 {
	return geometry.Scale(p, 2).Magnitude()
}
