/// Represents a user in the system.
pub struct User {
    pub name: String,
}

impl User {
    /// Returns a greeting message for the user.
    pub fn greet(&self) -> String {
        format!("Hello, {}!", self.name)
    }
}
