package greet

import "testing"

func TestMessage(t *testing.T) {
	if Message("Robin") != "Hello, Robin" {
		t.Fatal("wrong message")
	}
}
