-- Birthday lookups filter on the "D-M-" prefix of users.birthday.
CREATE INDEX IF NOT EXISTS users_birthday_prefix ON users (birthday text_pattern_ops) WHERE birthday <> '';
