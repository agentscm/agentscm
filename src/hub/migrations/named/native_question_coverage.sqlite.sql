-- Opaque native question sources whose current coverage is unavailable.
CREATE TABLE hub_native_question_coverage (
    owner_hash TEXT NOT NULL CHECK (length(owner_hash) = 64),
    scope_hash TEXT NOT NULL CHECK (length(scope_hash) = 64),
    native_hash TEXT NOT NULL CHECK (length(native_hash) = 64),
    -- Fixed-width decimal preserves the complete u64 range and its numeric order.
    rejected_offset TEXT NOT NULL DEFAULT '00000000000000000000'
        CHECK (length(rejected_offset) = 20 AND rejected_offset NOT GLOB '*[^0-9]*'),
    PRIMARY KEY (owner_hash, scope_hash, native_hash)
);
CREATE INDEX hub_native_question_coverage_clear ON hub_native_question_coverage (owner_hash, native_hash);
