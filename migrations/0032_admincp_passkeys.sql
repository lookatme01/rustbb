-- Bind Admin CP passkey challenges to their purpose and browser session.
ALTER TABLE webauthn_ceremonies ADD COLUMN context TEXT NOT NULL DEFAULT '';
