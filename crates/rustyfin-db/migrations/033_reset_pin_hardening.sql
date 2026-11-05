-- Migration 033: harden password reset PINs (R1-01, M-17).
-- PINs used to be 6 plaintext digits; now pin_code holds a SHA-256 hex of the normalized PIN.
-- Drop any legacy rows and add a per-reset failed-attempt counter (5-attempt cap).
DELETE FROM password_reset_pins;
ALTER TABLE password_reset_pins ADD COLUMN failed_attempts INTEGER NOT NULL DEFAULT 0;
