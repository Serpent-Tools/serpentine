from serpentine.nodes import ShellEscape

class Pipeline:
    def __init__(self):
        self.nodes = []

    def add_node(self, node):
        self.nodes.append(node)

    def run(self):
        output = None
        for node in self.nodes:
            output = node.run(output)
        return output

    def shell_escape(self, input):
        self.add_node(ShellEscape(input))
        return self.run()