class Greeter
  def initialize(name)
    @name = name
  end

  def greet
    "hello #{@name}"
  end
end

Greeter.new("world").greet
