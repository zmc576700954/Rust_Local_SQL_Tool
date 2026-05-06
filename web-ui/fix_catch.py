import os
import re

DIR = os.path.join(os.path.dirname(__file__), 'src')

def process_file(filepath):
    with open(filepath, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # Remove `: any` in catch block
    content = re.sub(r'catch\s*\(\s*(\w+)\s*:\s*any\s*\)', r'catch (\1)', content)
    
    # Replace `(\w+)\.message` to `(\1 as Error).message` inside catch blocks. We'll just do a global replace for `.message` and `.response` if the variable is e or err.
    # Actually, simpler: replace `e: any` with `e: any`? The goal is to remove `any`.
    # Let's just cast to `any` locally or use `(e as Error).message`.
    # Or replace `catch (e: any)` with `catch (_e)` and use `const e = _e as any;`
    content = re.sub(r'catch\s*\(\s*(\w+)\s*:\s*any\s*\)\s*\{', r'catch (\1: any) {', content) # Revert logic.
    
    with open(filepath, 'w', encoding='utf-8') as f:
        f.write(content)

if __name__ == '__main__':
    for root, _, files in os.walk(DIR):
        for file in files:
            if file.endswith('.tsx') or file.endswith('.ts'):
                filepath = os.path.join(root, file)
                process_file(filepath)
