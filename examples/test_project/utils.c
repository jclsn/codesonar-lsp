#include <stdlib.h>
#include <string.h>

#include "utils.h"

void copy_name(char *dest, const char *src)
{
    strcpy(dest, src);              /* no bounds check */
}

char *duplicate(const char *text)
{
    char *copy = malloc(strlen(text) + 1);

    strcpy(copy, text);             /* copy may be NULL */

    return copy;
}

int parse_number(const char *text)
{
    return atoi(text);              /* no error handling */
}

int compute(int a, int b, int c, int d, int e, int f, int g, int h)
{
    int unused = a;

    return a + b + c + d + e + f + g + h;
}
