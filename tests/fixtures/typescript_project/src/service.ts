import { User } from "./models";
import type { UserOptions, UserId } from "./models";

/**
 * Creates a new User instance.
 */
export function createUser(options: UserOptions): User {
  return new User(options);
}

/**
 * Finds a user by their ID (stub implementation for fixture).
 */
export function findUser(id: UserId): User | null {
  if (id === "1") {
    return createUser({ name: "Alice", email: "alice@example.com" });
  }
  return null;
}

/**
 * Returns a greeting for a user by ID.
 */
export function greetUser(id: UserId): string {
  const user = findUser(id);
  if (!user) return `User ${id} not found`;
  return user.greet();
}
