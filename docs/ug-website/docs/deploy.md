
## Create a new hosting site in firebase console
## firebase init
## Run below to deploy to the new site

firebase login
firebase deploy --only hosting:ultra-graph

## Host locally
python3 -m http.server or npx serve
http://localhost:8000/ or http://localhost:3000 