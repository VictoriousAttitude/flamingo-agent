cmake_minimum_required(VERSION 3.15)
project(logger_child LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

# Static MSVC runtime so the binary runs on a clean machine without the VC++ redistributable.
if(MSVC)
  set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
endif()

add_executable(logger-child src/main.cpp)
target_compile_options(logger-child PRIVATE
  $<$<CXX_COMPILER_ID:MSVC>:/W4 /permissive->
  $<$<NOT:$<CXX_COMPILER_ID:MSVC>>:-Wall -Wextra -Wpedantic>)

enable_testing()

add_test(NAME child_happy_path
  COMMAND logger-child --utc 2026-01-01T00:00:00.000Z --rss-bytes 42 --log-file ${CMAKE_BINARY_DIR}/happy.log)
set_tests_properties(child_happy_path PROPERTIES
  PASS_REGULAR_EXPRESSION "^2026-01-01T00:00:00.000Z rss_bytes=42 elevated=(true|false)")

# Asserts the documented exit code, not merely "non-zero" as WILL_FAIL would.
function(add_exit_code_test name expected)
  string(REPLACE ";" "\\;" args "${ARGN}")
  add_test(NAME ${name}
    COMMAND ${CMAKE_COMMAND}
      -DCHILD=$<TARGET_FILE:logger-child>
      "-DARGS=${args}"
      -DEXPECTED=${expected}
      -P ${CMAKE_SOURCE_DIR}/tests/exit_code_test.cmake)
endfunction()

# Missing --rss-bytes and an unknown flag are both usage errors -> exit 1.
add_exit_code_test(child_missing_rss 1 --utc 2026-01-01T00:00:00.000Z)
add_exit_code_test(child_unknown_flag 1 --utc x --rss-bytes 1 --bogus)

# A directory as the log file cannot be opened for writing -> exit 2.
add_exit_code_test(child_unwritable_log 2 --utc x --rss-bytes 1 --log-file ${CMAKE_BINARY_DIR})

add_test(NAME child_appends_never_truncates
  COMMAND ${CMAKE_COMMAND}
    -DCHILD=$<TARGET_FILE:logger-child>
    -DLOG=${CMAKE_BINARY_DIR}/append.log
    -P ${CMAKE_SOURCE_DIR}/tests/append_test.cmake)

# The happy-path regex accepts either elevation; this one pins it to the privilege the test
# process really has.
add_test(NAME child_reports_the_expected_elevation
  COMMAND ${CMAKE_COMMAND}
    -DCHILD=$<TARGET_FILE:logger-child>
    -DLOG=${CMAKE_BINARY_DIR}/elevation.log
    -P ${CMAKE_SOURCE_DIR}/tests/expected_elevation.cmake)

add_test(NAME child_default_log_path_is_used_when_flag_absent
  COMMAND ${CMAKE_COMMAND}
    -DCHILD=$<TARGET_FILE:logger-child>
    -DWORKDIR=${CMAKE_BINARY_DIR}
    -P ${CMAKE_SOURCE_DIR}/tests/default_path_test.cmake)
