"""User models for the lsp-cli test fixture."""

from dataclasses import dataclass
from typing import Optional


@dataclass
class User:
    """Represents a user in the system."""

    name: str
    email: Optional[str] = None

    def greet(self) -> str:
        """Returns a greeting message for the user."""
        return f"Hello, {self.name}!"

    def __str__(self) -> str:
        return f"User({self.name})"
