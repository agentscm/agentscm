-- requires: session_tool_activity
-- Historical starts retain the terminal fence after the ordinary session row is gone.
CREATE TABLE hub_session_incarnations (
    token TEXT NOT NULL REFERENCES hub_agent_tokens(id),
    session TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    start_nonce TEXT NOT NULL,
    ordinal BIGINT NOT NULL,
    qualification TEXT NOT NULL CHECK(qualification IN ('unknown','qualified','mixed')),
    state TEXT NOT NULL CHECK(state IN ('live','terminal')),
    observed_at BIGINT NOT NULL,
    register_operation TEXT NOT NULL,
    register_request TEXT NOT NULL,
    end_operation TEXT,
    end_request TEXT,
    end_result TEXT,
    PRIMARY KEY(token,session,incarnation),
    UNIQUE(token,session,start_nonce),
    UNIQUE(token,session,register_operation),
    UNIQUE(token,session,ordinal)
);
