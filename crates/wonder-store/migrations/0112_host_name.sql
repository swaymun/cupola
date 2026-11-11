-- The name the owner gave this computer in Wonder, from the Mac app or the
-- phone. Both show this one name; without a row Wonder uses the computer's
-- own name (macOS's ComputerName).
CREATE TABLE host_name (
  id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
  name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 60),
  updated_at TEXT NOT NULL
);
