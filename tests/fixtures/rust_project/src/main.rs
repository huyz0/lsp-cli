mod user;
use user::User;

fn main() {
    let u = User { name: "Alice".to_string() };
    println!("{}", u.greet());
}
