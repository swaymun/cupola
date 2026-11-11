-- Replace the old Codex-only backend choice with a computer-wide switch.
-- Both old choices permitted Cua Driver; neither meant off. Default to on.
CREATE TABLE computer_use_settings (
  id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
  cua_driver_enabled INTEGER NOT NULL CHECK (cua_driver_enabled IN (0,1)),
  updated_at TEXT NOT NULL
);
DELETE FROM provider_settings WHERE setting='desktop_computer_use';
