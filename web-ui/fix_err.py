import os
import re

DIR = os.path.join(os.path.dirname(__file__), 'src')

def process_file(filepath):
    with open(filepath, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # replace err.message with e.message in catch blocks (roughly)
    # wait, earlier it was `err.message` because the catch variable was `_err`. Now it is `err` but catch variable is `e`?
    # Let's just run sed to replace `err.message` with `e.message` in these files.
    content = content.replace('err.message', 'e.message')
    content = content.replace('err.response', 'e.response')
    content = content.replace('err.data', 'e.data')
    
    with open(filepath, 'w', encoding='utf-8') as f:
        f.write(content)

if __name__ == '__main__':
    for root, _, files in os.walk(DIR):
        for file in files:
            if file.endswith('.tsx') or file.endswith('.ts'):
                filepath = os.path.join(root, file)
                process_file(filepath)
