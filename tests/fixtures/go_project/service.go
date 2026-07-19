package main

// CreateUser creates a new User with the given name.
func CreateUser(name string) User {
	return User{Name: name}
}

// FindUser finds a user by ID (stub for fixture).
func FindUser(id string) *User {
	if id == "1" {
		u := CreateUser("Alice")
		u.Email = "alice@example.com"
		return &u
	}
	return nil
}

// GreetUser returns a greeting for a user by ID.
func GreetUser(id string) string {
	user := FindUser(id)
	if user == nil {
		return "User " + id + " not found"
	}
	return user.Greet()
}

func main() {}
