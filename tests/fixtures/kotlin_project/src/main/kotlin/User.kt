/** Represents a user in the system. */
class User(val name: String) {
    /** Returns a greeting message for the user. */
    fun greet(): String = "Hello, $name!"
}
