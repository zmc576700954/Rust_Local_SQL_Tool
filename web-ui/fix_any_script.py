import os
import re

DIR = os.path.join(os.path.dirname(__file__), 'src')

def process_file(filepath):
    with open(filepath, 'r', encoding='utf-8') as f:
        content = f.read()
    
    # fix catch(e: any)
    def catch_repl(m):
        var = m.group(1)
        return f"catch ({var}: unknown) {{\n      const _err = {var} as Record<string, unknown>;"
    
    content = re.sub(r'catch\s*\(\s*(\w+)\s*:\s*any\s*\)\s*\{', catch_repl, content)
    # replace the error variable
    content = re.sub(r'([^\w])(e|err)(\.message|\.response|\.data)', r'\1_err\3', content)
    
    # fix explicit any in arrays and objects
    content = re.sub(r'useState<any\[\]>', r'useState<Record<string, unknown>[]>', content)
    content = re.sub(r'useState<any>', r'useState<Record<string, unknown>>', content)
    content = re.sub(r':\s*any\[\]', r': Record<string, unknown>[]', content)
    content = re.sub(r':\s*any\b', r': Record<string, unknown>', content)
    content = re.sub(r'<\s*any\s*>', r'<Record<string, unknown>>', content)
    content = re.sub(r'as\s+any\b', r'as Record<string, unknown>', content)
    
    with open(filepath, 'w', encoding='utf-8') as f:
        f.write(content)

if __name__ == '__main__':
    for root, _, files in os.walk(DIR):
        for file in files:
            if file.endswith('.tsx') or file.endswith('.ts'):
                filepath = os.path.join(root, file)
                process_file(filepath)
