/**
 * User model for the lsp-cli test fixture.
 */

export interface UserOptions {
  name: string;
  email?: string;
}

/**
 * Represents a user in the system.
 */
export class User {
  readonly name: string;
  readonly email: string | undefined;

  constructor(options: UserOptions) {
    this.name = options.name;
    this.email = options.email;
  }

  /**
   * Returns a greeting message for the user.
   */
  greet(): string {
    return `Hello, ${this.name}!`;
  }

  /**
   * Returns a string representation of the user.
   */
  toString(): string {
    return `User(${this.name})`;
  }
}

export type UserId = string;
