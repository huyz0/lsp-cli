"""User service for the lsp-cli test fixture."""

from typing import Optional
from .models import User


def create_user(name: str, email: Optional[str] = None) -> User:
    """Creates a new User instance."""
    return User(name=name, email=email)


def find_user(user_id: str) -> Optional[User]:
    """Finds a user by their ID (stub for fixture)."""
    if user_id == "1":
        return create_user("Alice", "alice@example.com")
    return None


def greet_user(user_id: str) -> str:
    """Returns a greeting for a user by ID."""
    user = find_user(user_id)
    if user is None:
        return f"User {user_id} not found"
    return user.greet()
