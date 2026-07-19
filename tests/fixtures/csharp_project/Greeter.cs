namespace CsTest;

class Greeter
{
    private string name;

    public Greeter(string name)
    {
        this.name = name;
    }

    public string Greet()
    {
        return "hello " + name;
    }
}

class Program
{
    static void Main()
    {
        var g = new Greeter("world");
        g.Greet();
    }
}
