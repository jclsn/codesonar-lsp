CC ?= cc
CFLAGS ?= -std=c99 -Wall -g

test_project: main.o utils.o
	$(CC) $(CFLAGS) -o $@ $^

%.o: %.c utils.h
	$(CC) $(CFLAGS) -c -o $@ $<

clean:
	rm -f test_project *.o

.PHONY: clean
