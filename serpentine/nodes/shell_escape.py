from shellquote import quote

class ShellEscape:
    """
    Escapes a string for use in a shell.

    Args:
        input (str): The input string to escape.

    Returns:
        str: The escaped string.
    """

    def __init__(self, input):
        self.input = input

    def run(self):
        """
        Escapes the input string for use in a shell.

        Returns:
            str: The escaped string.
        """
        return quote(self.input)