namespace CsTest;

class Animal
{
    public virtual string Speak() => "...";
}

class Dog : Animal
{
    public override string Speak() => "Woof";
}

class Cat : Animal
{
    public override string Speak() => "Meow";
}
