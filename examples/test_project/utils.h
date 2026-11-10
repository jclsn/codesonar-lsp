#ifndef UTILS_H
#define UTILS_H

#define MAX_NAME 16
#define SQUARE(x) x * x             /* unparenthesized macro argument */

void copy_name(char *dest, const char *src);
char *duplicate(const char *text);
int parse_number(const char *text);
int compute(int a, int b, int c, int d, int e, int f, int g, int h);

#endif
