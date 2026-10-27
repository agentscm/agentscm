from collections import defaultdict, deque


class RateLimiter:
    """Rolling-window example; same account semantics, different valid algorithm."""

    def __init__(self):
        self.timestamps = defaultdict(deque)

    def allow(self, account_id, client_ip, now):
        recent = self.timestamps[account_id]
        while recent and now - recent[0] >= 60:
            recent.popleft()
        if len(recent) >= 2:
            return False
        recent.append(now)
        return True
