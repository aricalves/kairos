package report

import "example.com/shapes/shapes"

// Big gives the area of one square, twice.
func Big() float64 {
	sq := shapes.Square{Side: 2}
	return sq.Area() + shapes.Total([]shapes.Shape{sq})
}
