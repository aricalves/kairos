package hello

import (
	"fmt"

	"example.com/polyglot/go/greet"
	loud "example.com/polyglot/go/shout"
)

// Run greets 2 people.
func Run() {
	greet.Greet(loud.Message("Robin"))
	fmt.Println(greet.Message("Sam"))
}
