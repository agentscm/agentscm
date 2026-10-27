import unittest

from rate_limiter import RateLimiter


class RateLimiterTests(unittest.TestCase):
    def test_same_account_shares_limit_across_ips(self):
        limiter = RateLimiter()
        self.assertTrue(limiter.allow("acct-a", "192.0.2.1", 0))
        self.assertTrue(limiter.allow("acct-a", "198.51.100.2", 1))
        self.assertFalse(limiter.allow("acct-a", "203.0.113.3", 2))

    def test_different_accounts_have_independent_limits(self):
        limiter = RateLimiter()
        self.assertTrue(limiter.allow("acct-a", "192.0.2.1", 0))
        self.assertTrue(limiter.allow("acct-a", "198.51.100.2", 1))
        self.assertTrue(limiter.allow("acct-b", "203.0.113.3", 1))

    def test_allowance_resets_in_next_minute(self):
        limiter = RateLimiter()
        self.assertTrue(limiter.allow("acct-a", "192.0.2.1", 0))
        self.assertTrue(limiter.allow("acct-a", "198.51.100.2", 1))
        self.assertTrue(limiter.allow("acct-a", "203.0.113.3", 60))


if __name__ == "__main__":
    unittest.main()
