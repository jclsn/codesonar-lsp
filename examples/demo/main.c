#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "utils.h"

static int divide(int a, int b)
{
    return a / b;                /* possible division by zero */
}

static int *get_local_address(void)
{
    int local = 5;

    return &local;               /* returning address of a local */
}

int main(int argc, char *argv[])
{
    char name[MAX_NAME];
    char input[64];
    int *ptr = NULL;
    int i;
    int x = 0;

    if (argc > 1) {
        copy_name(name, argv[1]);
        printf(name);            /* format string vulnerability */
    }

    gets(input);                 /* dangerous, removed in C11 */

    char *dup = duplicate(input);
    free(dup);
    printf("%s\n", dup);         /* use after free */
    free(dup);                   /* double free */

    if (x = parse_number(input)) /* assignment in condition */
        printf("got %d\n", x);

    for (i = 0; i <= 10; i++);   /* empty loop body */

    printf("%d\n", SQUARE(x + 1));
    printf("%d\n", divide(10, 0));
    printf("%d\n", *get_local_address());

    if (argc == 3) {
        *ptr = 1;                /* null pointer dereference */
    }

    int *leak = malloc(100 * sizeof(int));  /* memory leak */
    leak[0] = compute(1, 2, 3, 4, 5, 6, 7, 8);

    goto end;                    /* goto usage */

end:

    return 0;
}
