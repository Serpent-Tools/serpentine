import unittest
from serpentine.nodes import ShellEscape

class TestShellEscape(unittest.TestCase):
    def test_empty_input(self):
        node = ShellEscape("")
        self.assertEqual(node.run(), "")

    def test_simple_input(self):
        node = ShellEscape("hello world")
        self.assertEqual(node.run(), "hello world")

    def test_special_chars(self):
        node = ShellEscape("hello; world")
        self.assertEqual(node.run(), "hello\\; world")

    def test_non_string_input(self):
        with self.assertRaises(TypeError):
            ShellEscape(123)

if __name__ == "__main__":
    unittest.main()