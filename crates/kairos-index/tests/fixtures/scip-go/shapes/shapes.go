package shapes

// Shape has an area.
type Shape interface {
	Area() float64
}

// Square is a shape.
type Square struct{ Side float64 }

// Area gives the area of the square.
func (s Square) Area() float64 { return s.Side * s.Side }

// Circle is a shape.
type Circle struct{ R float64 }

// Area gives the area of the circle.
func (c Circle) Area() float64 { return 3 * c.R * c.R }

// Total adds the areas of the shapes.
func Total(shapes []Shape) float64 {
	t := 0.0
	for _, s := range shapes {
		t += s.Area()
	}
	return t
}
