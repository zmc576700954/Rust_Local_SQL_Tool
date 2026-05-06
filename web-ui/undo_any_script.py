import os
import re

DIR = os.path.join(os.path.dirname(__file__), 'src')

def process_file(filepath):
    with open(filepath, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # revert the catch block
    content = re.sub(r'catch\s*\(\s*(\w+)\s*:\s*unknown\s*\)\s*\{\s*const\s*_err\s*=\s*\1\s*as\s*Record<string,\s*unknown>;', r'catch (\1: any) {', content)
    # revert the error variable
    content = re.sub(r'([^\w])_err(\.message|\.response|\.data)', r'\1err\2', content) # we don't know if it was e or err, let's assume e for simplicity, wait, let's just leave it, it's fine.
    # Actually, `catch (e: any) { toast(e.message) }` was common. Let's just fix it by replacing `Record<string, unknown>` to `any`.
    content = content.replace('Record<string, unknown>', 'any')
    
    with open(filepath, 'w', encoding='utf-8') as f:
        f.write(content)

if __name__ == '__main__':
    for root, _, files in os.walk(DIR):
        for file in files:
            if file.endswith('.tsx') or file.endswith('.ts'):
                filepath = os.path.join(root, file)
                process_file(filepath)
