-- Demonstrate soft deleted rows will not be return to queries
-- IOX_SETUP: OneDeleteSimpleExprOneChunk

-- select *
SELECT * from cpu;

SELECT time, bar from cpu;

SELECT min(bar), max(bar) from cpu;

SELECT time from cpu;

SELECT max(time)  from cpu;
SELECT min(time)  from cpu group by bar;
SELECT bar, min(time)  from cpu group by bar;

SELECT count(time), max(time)  from cpu;

SELECT count(time)  from cpu;

SELECT count(time), count(*), count(bar), min(bar), max(bar), min(time), max(time)  from cpu;

----------------------------------------------------------------
-- Now add selection predicate
SELECT * from cpu where bar = 2.0;

SELECT * from cpu where bar != 2.0;

SELECT count(time), count(*), count(bar), min(bar), max(bar), min(time), max(time)  from cpu where bar= 2.0;

SELECT count(time), count(*), count(bar), min(bar), max(bar), min(time), max(time)  from cpu where bar != 2.0;

SELECT time from cpu where bar=2;

SELECT bar from cpu where bar!= 2;


