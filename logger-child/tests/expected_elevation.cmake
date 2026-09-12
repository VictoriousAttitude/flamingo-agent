# Derives the elevation the child must report from the privilege this test process actually
# has, then asserts the happy-path line matches it exactly. `child_happy_path` accepts
# `elevated=(true|false)`, so a child that hard-coded either answer would still pass it; this
# test is what makes the "launched with administrator privileges" evidence trustworthy.
if(CMAKE_HOST_WIN32)
  execute_process(COMMAND whoami /groups OUTPUT_VARIABLE groups RESULT_VARIABLE probe)
  if(NOT probe EQUAL 0)
    message(FATAL_ERROR "whoami /groups failed: ${probe}")
  endif()
  # S-1-16-12288 is the High Mandatory Level integrity label, which only an elevated token
  # carries; the well-known SID is stable across locales, unlike the printed group name.
  if(groups MATCHES "S-1-16-12288")
    set(expected "true")
  else()
    set(expected "false")
  endif()
else()
  execute_process(COMMAND id -u
    OUTPUT_VARIABLE uid OUTPUT_STRIP_TRAILING_WHITESPACE RESULT_VARIABLE probe)
  if(NOT probe EQUAL 0)
    message(FATAL_ERROR "id -u failed: ${probe}")
  endif()
  if(uid STREQUAL "0")
    set(expected "true")
  else()
    set(expected "false")
  endif()
endif()

file(REMOVE "${LOG}")
execute_process(COMMAND "${CHILD}" --utc T --rss-bytes 1 --log-file "${LOG}"
  OUTPUT_VARIABLE out ERROR_VARIABLE err RESULT_VARIABLE code)
if(NOT code EQUAL 0)
  message(FATAL_ERROR "child exited with ${code}: ${err}")
endif()
string(STRIP "${out}" out)
if(NOT out STREQUAL "T rss_bytes=1 elevated=${expected}")
  message(FATAL_ERROR "expected 'T rss_bytes=1 elevated=${expected}', got '${out}'")
endif()
