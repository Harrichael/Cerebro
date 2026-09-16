package geometry

func Scale(p Point, k float64) Point {
	return Point{X: p.X * k, Y: p.Y * k}
}
