package com.example;

/** Represents a user in the system. */
public class User {
    private final String name;

    public User(String name) {
        this.name = name;
    }

    /** Returns a greeting message for the user. */
    public String greet() {
        return "Hello, " + name + "!";
    }
}
