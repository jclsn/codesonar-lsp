# Generates codesonar.sarif for main.c: python3 gen_sarif.py main.c codesonar.sarif
import json, sys

src = open(sys.argv[1]).read().split("\n")
FILE = {"uri": "main.c"}

def region(line, text, nth=0):
    """Region of the nth occurrence of text in a 1-based line; endColumn is exclusive."""
    s = src[line - 1]
    col = -1
    for _ in range(nth + 1):
        col = s.index(text, col + 1)
    assert col >= 0, (line, text)
    return {"startLine": line, "startColumn": col + 1, "endLine": line, "endColumn": col + 1 + len(text)}

def loc(line, text, nth=0, message=None):
    l = {"physicalLocation": {"artifactLocation": FILE, "region": region(line, text, nth)}}
    if message:
        l["message"] = {"text": message}
    return l

def fix(description, reg, text):
    return {"description": {"text": description},
            "artifactChanges": [{"artifactLocation": FILE,
                                 "replacements": [{"deletedRegion": reg, "insertedContent": {"text": text}}]}]}

HUB = "http://nas:7340"
rules = [
    ("LANG.ARITH.DIVZERO", "Division By Zero", "error",
     "The divisor of a division or modulo operation can be zero.",
     "Dividing by zero is **undefined behavior** in C. Make sure the divisor is not zero before dividing, for example with an explicit check."),
    ("LANG.STRUCT.RPL", "Return Pointer To Local", "error",
     "A function returns the address of a local variable.",
     "The lifetime of a local variable ends when the function returns, so the returned pointer is **dangling**. Return the value itself, use a caller-provided buffer, or allocate on the heap."),
    ("IO.INJ.FMT", "Format String Injection", "warning",
     "Data that may be controlled by the user is used as a format string.",
     "If the format string contains conversion specifiers like `%s` or `%n`, `printf` reads or writes arbitrary memory. Always use a constant format string: `printf(\"%s\", name)`."),
    ("BADFUNC.GETS", "Use of gets", "error",
     "The function gets is used.",
     "`gets` cannot limit the number of characters it reads and always allows a **buffer overrun**. It was removed in C11. Use `fgets(buffer, sizeof(buffer), stdin)` instead."),
    ("ALLOC.UAF", "Use After Free", "error",
     "Memory is accessed after it has been freed.",
     "After `free`, the memory may be reused for other data. Reading or writing it is **undefined behavior** and a common source of security vulnerabilities."),
    ("ALLOC.DF", "Double Free", "error",
     "Memory is freed twice.",
     "Freeing the same pointer twice corrupts the heap and can be exploited. Set pointers to `NULL` after freeing them or make sure each allocation is freed exactly once."),
    ("LANG.STRUCT.CONDASSIG", "Assignment In Conditional", "note",
     "An assignment is used as the condition of an if statement.",
     "This is often a typo for `==`. If the assignment is intended, make it explicit: `if ((x = parse_number(input)) != 0)`."),
    ("LANG.STRUCT.EBS", "Empty Loop Body", "note",
     "A for loop has an empty body.",
     "The semicolon directly after the loop header makes the loop body empty. This is often unintended. If it is intended, write an empty block `{}` to make that clear."),
    ("LANG.MEM.NPD", "Null Pointer Dereference", "error",
     "A pointer that can be NULL is dereferenced.",
     "Dereferencing a null pointer is **undefined behavior** and usually crashes the program. Check the pointer before use."),
    ("ALLOC.LEAK", "Leak", "warning",
     "Allocated memory is not freed.",
     "Memory that is allocated but never freed is lost until the program exits. Free each allocation when it is no longer needed."),
    ("LANG.STRUCT.GOTO", "Use of goto", "note",
     "A goto statement is used.",
     "`goto` makes the control flow hard to follow. Prefer structured control flow such as loops, `break` and early returns."),
]
rule_index = {r[0]: i for i, r in enumerate(rules)}

results = []
def result(rule, message, location, related=(), fixes=()):
    n = len(results) + 1
    r = {"ruleId": rule, "ruleIndex": rule_index[rule], "message": {"text": message},
         "locations": [location], "hostedViewerUri": f"{HUB}/warninginstance/{n}.html"}
    if related: r["relatedLocations"] = list(related)
    if fixes: r["fixes"] = list(fixes)
    results.append(r)

result("LANG.ARITH.DIVZERO", "Division by zero: b is 0 when called from main",
       loc(9, "a / b"), [loc(45, "divide(10, 0)", message="divide is called with b = 0")])
result("LANG.STRUCT.RPL", "Returning the address of local variable 'local'",
       loc(16, "&local"), [loc(14, "local", message="'local' is declared here")])
result("IO.INJ.FMT", "Format string 'name' comes from argv[1]",
       loc(29, "name"), [loc(28, "argv[1]", message="tainted data enters here")],
       [fix("Use a constant format string", region(29, "printf(name)"), 'printf("%s", name)')])
result("BADFUNC.GETS", "Use of gets",
       loc(32, "gets"), fixes=[fix("Replace gets with fgets", region(32, "gets(input)"), "fgets(input, sizeof(input), stdin)")])
result("ALLOC.UAF", "'dup' is used after it was freed",
       loc(36, "dup"), [loc(34, "duplicate(input)", message="memory is allocated here"),
                         loc(35, "free(dup)", message="memory is freed here")])
result("ALLOC.DF", "'dup' is freed twice",
       loc(37, "free(dup)"), [loc(35, "free(dup)", message="first free")],
       [fix("Remove the second free",
            {"startLine": 37, "startColumn": 1, "endLine": 38, "endColumn": 1}, "")])
result("LANG.STRUCT.CONDASSIG", "Assignment 'x = parse_number(input)' used as condition",
       loc(39, "x = parse_number(input)"),
       fixes=[fix("Compare the assignment explicitly", region(39, "x = parse_number(input)"), "(x = parse_number(input)) != 0")])
result("LANG.STRUCT.EBS", "The body of this for loop is empty",
       loc(42, ";", 2), fixes=[fix("Use an explicit empty block", region(42, ";", 2), " {}")])
result("LANG.MEM.NPD", "Null pointer dereference of 'ptr'",
       loc(49, "*ptr"), [loc(23, "ptr = NULL", message="'ptr' is set to NULL here")])
result("LANG.MEM.NPD", "'leak' may be NULL because malloc can fail",
       loc(53, "leak[0]"), [loc(52, "malloc(100 * sizeof(int))", message="malloc may return NULL")])
result("ALLOC.LEAK", "Memory allocated for 'leak' is never freed",
       loc(52, "malloc(100 * sizeof(int))"),
       fixes=[fix("Free 'leak' before leaving main",
                  {"startLine": 55, "startColumn": 5, "endLine": 55, "endColumn": 5}, "free(leak);\n    ")])
result("LANG.STRUCT.GOTO", "Use of goto",
       loc(55, "goto"))

log = {
    "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
    "version": "2.1.0",
    "runs": [{
        "tool": {"driver": {
            "name": "CodeSonar",
            "rules": [{"id": i, "name": n, "shortDescription": {"text": n},
                       "fullDescription": {"text": f}, "help": {"text": h.replace("**", "").replace("`", ""), "markdown": h},
                       "defaultConfiguration": {"level": lv}} for i, n, lv, f, h in rules],
        }},
        "invocations": [{"executionSuccessful": True, "endTimeUtc": "2026-10-22T10:00:00Z"}],
        "results": results,
    }],
}
json.dump(log, open(sys.argv[2], "w"), indent=2)
print(len(results), "results")
