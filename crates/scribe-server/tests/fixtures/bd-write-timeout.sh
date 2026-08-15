#!/bin/sh
sleep 30 &
echo $! > child.pid
wait
