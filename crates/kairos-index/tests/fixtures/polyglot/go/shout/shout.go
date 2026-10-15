package shout

import "strings"

// Message builds a loud greeting.
func Message(name string) string {
	return strings.ToUpper(name)
}
