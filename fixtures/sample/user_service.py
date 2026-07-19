class UserService:
    def __init__(self, db):
        self.db = db

    def create_user(self, name, email):
        user = User(name, email)
        self.db.save(user)
        return user

    def find_user_by_email(self, email):
        return self.db.query(email=email)


class User:
    def __init__(self, name, email):
        self.name = name
        self.email = email
