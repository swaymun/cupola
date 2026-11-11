-- T3 Code's title (owner, Oct 22: "use the titles that T3 Code uses for
-- threads"): the title T3 Code had for the thread when Cupola last showed it.
-- A thread T3 Code has takes T3 Code's title each time that title changes; a
-- rename in Cupola stands until T3 Code's title changes again.
ALTER TABLE project_conversations ADD COLUMN t3_title TEXT;
