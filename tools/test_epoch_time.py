import unittest

from epoch_time import build_epochs, is_dated, latest_unix, make_unix_s


class RingClockTests(unittest.TestCase):
    def test_rtc_beacon_dates_sleep_independently_of_download_time(self):
        for previous_boot in ([], [(5_000_000, 0x42, '{"unix_time":1788800000}', 1_788_800_000)]):
            with self.subTest(previous_boot=bool(previous_boot)):
                epochs = build_epochs(previous_boot + [
                    (672_400, 1, '{}', 1_789_056_420),
                    (1_000_000, 0x85, '{"unix_time":1789020420}', 1_789_056_420),
                ])
                unix_s = make_unix_s(epochs)
                # UTC+1: Sep 9 22:01 to Sep 10 07:07, downloaded at 17:07.
                self.assertEqual(unix_s(672_400, 1_789_056_420), 1_788_987_660)
                self.assertEqual(unix_s(1_000_000, 1_789_056_420), 1_789_020_420)
                self.assertEqual(latest_unix(epochs), 1_789_020_420)
                if previous_boot:
                    self.assertEqual(unix_s(5_000_000, 1_788_800_000), 1_788_800_000)


    def test_new_boot_is_not_projected_through_old_boot_nor_dated_to_download(self):
        epochs = build_epochs([
            (5_000_000, 0x42, '{"unix_time":1788800000}', 1_788_800_000),
            (5_100_000, 1, '{}', 1_788_800_000),
            (10, 1, '{}', 1_789_056_420),
            (300_000, 0x76, '{}', 1_789_056_420),
        ])
        self.assertFalse(is_dated(epochs, 300_000, 1_789_056_420))
        self.assertNotEqual(make_unix_s(epochs)(300_000, 1_789_056_420),
                            1_788_800_000 + (300_000 - 5_000_000) / 10.0)
        self.assertTrue(is_dated(epochs, 5_100_000, 1_788_800_000))

    def test_phone_anchor_dates_new_boot(self):
        epochs = build_epochs([
            (5_000_000, 0x42, '{"unix_time":1788800000}', 1_788_800_000),
            (10, 1, '{}', 1_789_056_420),
            (705_000, 0x42, '{"unix_time":1789056420,"source":"phone"}', 1_789_056_420),
        ])
        start = 705_000 - (1_789_056_420 - 1_789_002_000) * 10
        self.assertTrue(is_dated(epochs, start, 1_789_056_420))
        self.assertEqual(make_unix_s(epochs)(start, 1_789_056_420), 1_789_002_000)


if __name__ == '__main__':
    unittest.main()
