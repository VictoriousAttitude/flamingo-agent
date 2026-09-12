# With no --log-file the child must fall back to the platform default path. The two
# platforms can only be observed differently: on Windows %ProgramData% is redirectable, so
# the file itself is checked; on Unix the path is fixed and unwritable for a normal user, so
# the documented exit code 2 and the path named in the diagnostic are checked instead.
if(CMAKE_HOST_WIN32)
  set(program_data "${WORKDIR}/programdata")
  file(REMOVE_RECURSE "${program_data}")
  # In production the agent creates and locks down this directory before the child runs; the
  # child only ever appends, so it has to exist here too.
  file(MAKE_DIRECTORY "${program_data}/FlamingoAgent")
  set(ENV{ProgramData} "${program_data}")
  execute_process(COMMAND "${CHILD}" --utc T --rss-bytes 1
    OUTPUT_VARIABLE out ERROR_VARIABLE err RESULT_VARIABLE code)
  if(NOT code EQUAL 0)
    message(FATAL_ERROR "expected exit 0, got ${code}: ${err}")
  endif()
  if(NOT EXISTS "${program_data}/FlamingoAgent/child.log")
    message(FATAL_ERROR "child.log was not created under ${program_data}/FlamingoAgent")
  endif()
else()
  execute_process(COMMAND id -u OUTPUT_VARIABLE uid OUTPUT_STRIP_TRAILING_WHITESPACE)
  if(uid STREQUAL "0")
    message(STATUS "skipped: running as root, /var/log/flamingo-agent is writable")
    return()
  endif()
  execute_process(COMMAND "${CHILD}" --utc T --rss-bytes 1
    OUTPUT_VARIABLE out ERROR_VARIABLE err RESULT_VARIABLE code)
  if(NOT code EQUAL 2)
    message(FATAL_ERROR "expected exit 2, got ${code} (stderr: ${err})")
  endif()
  if(NOT err MATCHES "/var/log/flamingo-agent/child\\.log")
    message(FATAL_ERROR "stderr does not name the default log path: ${err}")
  endif()
endif()
