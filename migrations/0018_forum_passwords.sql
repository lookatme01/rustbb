-- Forum passwords become Argon2id verifiers (plaintext values are hashed by `rbb` at startup,
-- see install::upgrade). Unlock cookies are bound to password_version, which changes whenever
-- the password does, so changing or removing a password signs everyone out of the forum
-- without the cookie depending on the password itself.
ALTER TABLE forums ADD COLUMN password_version INT NOT NULL DEFAULT 0;
