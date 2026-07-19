// Package main provides user models for the lsp-cli test fixture.
package main

// User represents a user in the system.
type User struct {
	Name  string
	Email string
}

// Greet returns a greeting message for the user.
func (u User) Greet() string {
	return "Hello, " + u.Name + "!"
}

// String returns a string representation of the user.
func (u User) String() string {
	return "User(" + u.Name + ")"
}
