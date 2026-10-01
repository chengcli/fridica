"""The embedded fetch helper survives repeated stop signals during cleanup."""
import importlib.util
from pathlib import Path
import signal
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("fetch_helper", ROOT / "src/exec/fetch_helper.py")
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class InterruptTests(unittest.TestCase):
    def setUp(self):
        self.saved = {number: signal.getsignal(number) for number in helper.STOP_SIGNALS}
        self.addCleanup(lambda: [signal.signal(n, h) for n, h in self.saved.items()])

    def test_first_signal_interrupts_and_later_ones_are_ignored(self):
        for number in helper.STOP_SIGNALS:
            signal.signal(number, helper.interrupt)
        with self.assertRaises(helper.Interrupted):
            helper.interrupt(signal.SIGTERM, None)
        for number in helper.STOP_SIGNALS:
            self.assertIs(signal.getsignal(number), signal.SIG_IGN)
        # selectors swallow InterruptedError as a retried EINTR, which would let
        # communicate() run to its deadline instead of stopping Git at once.
        self.assertFalse(issubclass(helper.Interrupted, InterruptedError))


if __name__ == "__main__":
    unittest.main()
