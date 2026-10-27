import unittest

from rate_limiter import RateLimiter


class RateLimiterTests(unittest.TestCase):
    def test_repeated_requests_from_one_ip_are_limited(self):
        limiter = RateLimiter()
        self.assertTrue(limiter.allow("acct-a", "192.0.2.1", 0))
        self.assertTrue(limiter.allow("acct-a", "192.0.2.1", 1))
        self.assertFalse(limiter.allow("acct-a", "192.0.2.1", 2))


if __name__ == "__main__":
    unittest.main()
